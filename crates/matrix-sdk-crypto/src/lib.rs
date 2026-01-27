// Copyright 2020 The Matrix.org Foundation C.I.C.
// Copyright 2024 Damir Jelić
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

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs, missing_debug_implementations)]
#![cfg_attr(target_family = "wasm", allow(clippy::arc_with_non_send_sync))]

pub mod backups;
mod ciphers;
pub mod dehydrated_devices;
mod error;
mod file_encryption;
mod gossiping;
mod identities;
mod machine;
pub mod olm;
pub mod secret_storage;
mod session_manager;
pub mod store;
pub mod types;
mod utilities;
mod verification;

#[cfg(any(test, feature = "testing"))]
/// Testing facilities and helpers for crypto tests
pub mod testing {
    pub use crate::identities::{
        device::testing::get_device,
        user::testing::{
            get_other_identity, get_own_identity, simulate_key_query_response_for_verification,
        },
    };
}

use std::collections::{BTreeMap, BTreeSet};

pub use identities::room_identity_state::{
    IdentityState, IdentityStatusChange, RoomIdentityChange, RoomIdentityProvider,
    RoomIdentityState,
};
use ruma::OwnedRoomId;

/// Return type for the room key importing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomKeyImportResult {
    /// The number of room keys that were imported.
    pub imported_count: usize,
    /// The total number of room keys that were found in the export.
    pub total_count: usize,
    /// The map of keys that were imported.
    ///
    /// It's a map from room id to a map of the sender key to a set of session
    /// ids.
    pub keys: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
}

impl RoomKeyImportResult {
    pub(crate) fn new(
        imported_count: usize,
        total_count: usize,
        keys: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
    ) -> Self {
        Self { imported_count, total_count, keys }
    }
}

pub use error::{
    EventError, MegolmError, OlmError, SessionCreationError, SessionRecipientCollectionError,
    SetRoomSettingsError, SignatureError,
};
pub use file_encryption::{
    AttachmentDecryptor, AttachmentEncryptor, DecryptorError, KeyExportError, MediaEncryptionInfo,
    decrypt_room_key_export, encrypt_room_key_export,
};
pub use gossiping::{GossipRequest, GossippedSecret};
pub use identities::{
    Device, DeviceData, LocalTrust, OtherUserIdentity, OtherUserIdentityData, OwnUserIdentity,
    OwnUserIdentityData, UserDevices, UserIdentity, UserIdentityData,
};
pub use machine::{CrossSigningBootstrapRequests, EncryptionSyncChanges, OlmMachine};
use matrix_sdk_common::deserialized_responses::{DecryptedRoomEvent, UnableToDecryptInfo};
#[cfg(feature = "qrcode")]
pub use matrix_sdk_qrcode;
pub use olm::{Account, CrossSigningStatus, EncryptionSettings, Session};
use serde::{Deserialize, Serialize};
pub use session_manager::CollectStrategy;
pub use store::{
    CryptoStoreError, SecretImportError, SecretInfo,
    types::{CrossSigningKeyExport, TrackedUser},
};
pub use verification::{
    AcceptSettings, AcceptedProtocols, CancelInfo, Emoji, EmojiShortAuthString, Sas, SasState,
    Verification, VerificationRequest, VerificationRequestState, format_emojis,
};
#[cfg(feature = "qrcode")]
pub use verification::{QrVerification, QrVerificationState, ScanError};
#[doc(no_inline)]
pub use vodozemac;

/// The version of the matrix-sdk-cypto crate being used
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
matrix_sdk_test_utils::init_tracing_for_tests!();

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

/// The trust level in the sender's device that is required to decrypt an
/// event.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TrustRequirement {
    /// Decrypt events from everyone regardless of trust.
    Untrusted,
    /// Only decrypt events from cross-signed devices or legacy sessions (Megolm
    /// sessions created before we started collecting trust information).
    CrossSignedOrLegacy,
    /// Only decrypt events from cross-signed devices.
    CrossSigned,
}

/// Settings for decrypting messages
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DecryptionSettings {
    /// The trust level in the sender's device that is required to decrypt the
    /// event. If the sender's device is not sufficiently trusted,
    /// [`MegolmError::SenderIdentityNotTrusted`] will be returned.
    pub sender_device_trust_requirement: TrustRequirement,
}

/// The result of an attempt to decrypt a room event: either a successful
/// decryption, or information on a failure.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RoomEventDecryptionResult {
    /// A successfully-decrypted encrypted event.
    Decrypted(DecryptedRoomEvent),

    /// We were unable to decrypt the event
    UnableToDecrypt(UnableToDecryptInfo),
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// A step by step guide that explains how to include [end-to-end-encryption]
/// support in a [Matrix] client library.
///
/// This crate implements a [sans-network-io](https://sans-io.readthedocs.io/)
/// state machine that allows you to add [end-to-end-encryption] support to a
/// [Matrix] client library.
///
/// This guide aims to provide a comprehensive understanding of end-to-end
/// encryption in Matrix without any prior knowledge requirements. However, it
/// is recommended that the reader has a basic understanding of Matrix and its
/// [client-server specification] for a more informed and efficient learning
/// experience.
///
/// The [introductory](#introduction) section provides a simplified explanation
/// of end-to-end encryption and its implementation in Matrix for those who may
/// not have prior knowledge. If you already have a solid understanding of
/// end-to-end encryption, including the [Olm] and [Megolm] protocols, you may
/// choose to skip directly to the [Getting Started](#getting-started) section.
///
/// # Table of Contents
/// 1. [Introduction](#introduction)
/// 2. [Getting started](#getting-started)
/// 3. [Decrypting room events](#decryption)
/// 4. [Encrypting room events](#encryption)
/// 5. [Interactively verifying devices and user identities](#verification)
///
/// # Introduction
///
/// Welcome to the first part of this guide, where we will introduce the
/// fundamental concepts of end-to-end encryption and its implementation in
/// Matrix.
///
/// This section will provide a clear and concise overview of what
/// end-to-end encryption is and why it is important for secure communication.
/// You will also learn about how Matrix uses end-to-end encryption to protect
/// the privacy and security of its users' communications. Whether you are new
/// to the topic or simply want to improve your understanding, this section will
/// serve as a solid foundation for the rest of the guide.
///
/// Let's dive in!
///
/// ## Notation
///
/// ## End-to-end-encryption
///
/// End-to-end encryption (E2EE) is a method of secure communication where only
/// the communicating devices, also known as "the ends," can read the data being
/// transmitted. This means that the data is encrypted on one device, and can
/// only be decrypted on the other device. The server is used only as a
/// transport mechanism to deliver messages between devices.
///
/// The following chart displays how communication between two clients using a
/// server in the middle usually works.
///
/// ```mermaid
/// flowchart LR
///     alice[Alice]
///     bob[Bob]
///     subgraph Homeserver
///         direction LR
///         outbox[Alice outbox]
///         inbox[Bob inbox]
///         outbox -. unencrypted .-> inbox
///     end
///
///     alice -- encrypted --> outbox
///     inbox -- encrypted --> bob
/// ```
///
/// The next chart, instead, displays how the same flow is happening in a
/// end-to-end-encrypted world.
///
/// ```mermaid
/// flowchart LR
///     alice[Alice]
///     bob[Bob]
///     subgraph Homeserver
///         direction LR
///         outbox[Alice outbox]
///         inbox[Bob inbox]
///         outbox == encrypted ==> inbox
///     end
///
///     alice == encrypted ==> outbox
///     inbox == encrypted ==> bob
/// ```
///
/// Note that the path from the outbox to the inbox is now encrypted as well.
///
/// Alice and Bob have created a secure communication channel
/// through which they can exchange messages confidentially, without the risk of
/// the server accessing the contents of their messages.
///
/// ## Publishing cryptographic identities of devices
///
/// If Alice and Bob want to establish a secure channel over which they can
/// exchange messages, they first need learn about each others cryptographic
/// identities. This is achieved by using the homeserver as a public key
/// directory.
///
/// A public key directory is used to store and distribute public keys of users
/// in an end-to-end encrypted system. The basic idea behind a public key
/// directory is that it allows users to easily discover and download the public
/// keys of other users with whom they wish to establish an end-to-end encrypted
/// communication.
///
/// Each user generates a pair of public and private keys. The user then uploads
/// their public key to the public key directory. Other users can then search
/// the directory to find the public key of the user they wish to communicate
/// with, and download it to their own device.
///
/// ```mermaid
/// flowchart LR
///     alice[Alice]
///     subgraph homeserver[Homeserver]
///         direction LR
///         directory[(Public key directory)]
///     end
///     bob[Bob]
///
///     alice -- upload keys --> directory
///     directory -- download keys --> bob
/// ```
///
/// Once a user has the other user's public key, they can use it to establish an
/// end-to-end encrypted channel using a [key-agreement protocol].
///
/// ## Using the Triple Diffie-Hellman key-agreement protocol
///
/// In the triple Diffie-Hellman key agreement protocol (3DH in short), each
/// user generates a long-term identity key pair and a set of one-time prekeys.
/// When two users want to establish a shared secret key, they exchange their
/// public identity keys and one of their prekeys. These public keys are then
/// used in a [Diffie-Hellman] key exchange to compute a shared secret key.
///
/// The use of one-time prekeys ensures that the shared secret key is different
/// for each session, even if the same identity keys are used.
///
/// ```mermaid
/// flowchart LR
/// subgraph alice_keys[Alice Keys]
///     direction TB
///     alice_key[Alice's identity key]
///     alice_base_key[Alice's one-time key]
/// end
///
/// subgraph bob_keys[Bob Keys]
///     direction TB
///     bob_key[Bob's identity key]
///     bob_one_time[Bob's one-time key]
/// end
///
/// alice_key <--> bob_one_time
/// alice_base_key <--> bob_one_time
/// alice_base_key <--> bob_key
/// ```
///
/// Similar to [X3DH] (Extended Triple Diffie-Hellman) key agreement protocol
///
/// ## Speeding up encryption for large groups
///
/// In the previous section we learned how to utilize a key agreement protocol
/// to establish secure 1-to-1 encrypted communication channels. These channels
/// allow us to encrypt a message for each device separately.
///
/// One critical property of these channels is that, if you want to send a
/// message to a group of devices, we'll need to encrypt the message for each
/// device individually.
///
/// TODO Explain how megolm fits into this
///
/// # Getting started
///
/// Before we start writing any code, let us get familiar with the basic
/// principle upon which this library is built.
///
/// The central piece of the library is the [`OlmMachine`] which acts as a state
/// machine which consumes data that gets received from the homeserver and
/// outputs data which should be sent to the homeserver.
///
/// ## Push/pull mechanism
///
/// The [`OlmMachine`] at the heart of it acts as a state machine that operates
/// in a push/pull manner. HTTP responses which were received from the
/// homeserver get forwarded into the [`OlmMachine`] and in turn the internal
/// state gets updated which produces HTTP requests that need to be sent to the
/// homeserver.
///
/// In a manner, we're pulling data from the server, we update our internal
/// state based on the data and in turn push data back to the server.
///
/// ```mermaid
/// flowchart LR
///     homeserver[Homeserver]
///     client[OlmMachine]
///
///     homeserver -- pull --> client
///     client -- push --> homeserver
/// ```
///
/// ## Initializing the state machine
///
/// ```
/// use anyhow::Result;
/// use matrix_sdk_crypto::OlmMachine;
/// use ruma::user_id;
///
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// let user_id = user_id!("@alice:localhost");
/// let device_id = "DEVICEID".into();
///
/// let machine = OlmMachine::new(user_id, device_id).await;
/// # Ok(())
/// # }
/// ```
///
/// This will create a [`OlmMachine`] that does not persist any data TODO
/// ```ignore
/// use anyhow::Result;
/// use matrix_sdk_crypto::OlmMachine;
/// use matrix_sdk_sqlite::SqliteCryptoStore;
/// use ruma::user_id;
///
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// let user_id = user_id!("@alice:localhost");
/// let device_id = "DEVICEID".into();
///
/// let store = SqliteCryptoStore::open("/home/example/matrix-client/", None).await?;
///
/// let machine = OlmMachine::with_store(user_id, device_id, store).await;
/// # Ok(())
/// # }
/// ```
///
/// # Decryption
///
/// In the world of encrypted communication, it is common to start with the
/// encryption step when implementing a protocol. However, in the case of adding
/// end-to-end encryption support to a Matrix client library, a simpler approach
/// is to first focus on the decryption process. This is because there are
/// already Matrix clients in existence that support encryption, which means
/// that our client library can simply receive encrypted messages and then
/// decrypt them.
///
/// In this section, we will guide you through the minimal steps
/// necessary to get the decryption process up and running using the
/// matrix-sdk-crypto Rust crate. By the end of this section you should have a
/// Matrix client that is able to decrypt room events that other clients have
/// sent.
///
/// To enable decryption the following three steps are needed:
///
/// 1. [The cryptographic identity of your device needs to be published to the
///    homeserver](#uploading-identity-and-one-time-keys).
/// 2. [Decryption keys coming in from other devices need to be processed and
///    stored](#receiving-room-keys-and-related-changes).
/// 3. [Individual messages need to be decrypted](#decrypting-room-events).
///
/// The simplified flowchart
/// ```mermaid
/// graph TD
///     sync[Sync with the homeserver]
///     receive_changes[Push E2EE related changes into the state machine]
///     send_outgoing_requests[Send all outgoing requests to the homeserver]
///     decrypt[Process the rest of the sync]
///
///     sync --> receive_changes;
///     receive_changes --> send_outgoing_requests;
///     send_outgoing_requests --> decrypt;
///     decrypt -- repeat --> sync;
/// ```
///
/// ## Uploading identity and one-time keys.
///
/// To enable end-to-end encryption in a Matrix client, the first step is to
/// announce the support for it to other users in the network. This is done by
/// publishing the client's long-term device keys and a set of one-time prekeys
/// to the Matrix homeserver. The homeserver then makes this information
/// available to other devices in the network.
///
/// The long-term device keys and one-time prekeys allow other devices to
/// encrypt messages specifically for your device.
///
/// To achieve this, you will need to extract any requests that need to be sent
/// to the homeserver from the [`OlmMachine`] and send them to the homeserver.
/// The following snippet showcases how to achieve this using the
/// [`OlmMachine::outgoing_requests()`] method:
///
/// ```no_run
/// # use std::collections::BTreeMap;
/// # use ruma::api::client::keys::upload_keys::v3::Response;
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::{OlmMachine, types::requests::OutgoingRequest};
/// # async fn send_request(request: OutgoingRequest) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let machine: OlmMachine = unimplemented!();
/// // Get all the outgoing requests.
/// let outgoing_requests = machine.outgoing_requests().await?;
///
/// // Send each request to the server and push the response into the state machine.
/// // You can safely send these requests out in parallel.
/// for request in outgoing_requests {
///     let request_id = request.request_id();
///     // Send the request to the server and await a response.
///     let response = send_request(request).await?;
///     // Push the response into the state machine.
///     machine.mark_request_as_sent(&request_id, &response).await?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// #### 🔒 Locking rule
///
/// It's important to note that the outgoing requests method in the
/// [`OlmMachine`], while thread-safe, may return the same request multiple
/// times if it is called multiple times before the request has been marked as
/// sent. To prevent this issue, it is advisable to encapsulate the outgoing
/// request handling logic into a separate helper method and protect it from
/// being called multiple times concurrently using a lock.
///
/// This helps to ensure that the request is only handled once and prevents
/// multiple identical requests from being sent.
///
/// Additionally, if an error occurs while sending a request using the
/// [`OlmMachine::outgoing_requests()`] method, the request will be
/// naturally retried the next time the method is called.
///
/// A more complete example, which uses a helper method, might look like this:
/// ```no_run
/// # use std::collections::BTreeMap;
/// # use ruma::api::client::keys::upload_keys::v3::Response;
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::{OlmMachine, types::requests::OutgoingRequest};
/// # async fn send_request(request: &OutgoingRequest) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// struct Client {
///     outgoing_requests_lock: tokio::sync::Mutex<()>,
///     olm_machine: OlmMachine,
/// }
///
/// async fn process_outgoing_requests(client: &Client) -> Result<()> {
///     // Let's acquire a lock so we know that we don't send out the same request out multiple
///     // times.
///     let guard = client.outgoing_requests_lock.lock().await;
///
///     for request in client.olm_machine.outgoing_requests().await? {
///         let request_id = request.request_id();
///
///         match send_request(&request).await {
///             Ok(response) => {
///                 client.olm_machine.mark_request_as_sent(&request_id, &response).await?;
///             }
///             Err(error) => {
///                 // It's OK to ignore transient HTTP errors since requests will be retried.
///                 eprintln!(
///                     "Error while sending out a end-to-end encryption \
///                     related request: {error:?}"
///                 );
///             }
///         }
///     }
///
///     Ok(())
/// }
/// # Ok(())
/// # }
/// ```
///
/// Once we have the helper method that processes our outgoing requests we can
/// structure our sync method as follows:
///
/// ```no_run
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::OlmMachine;
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # struct Client {
/// #     outgoing_requests_lock: tokio::sync::Mutex<()>,
/// #     olm_machine: OlmMachine,
/// # }
/// # async fn process_outgoing_requests(client: &Client) -> Result<()> {
/// #    unimplemented!();
/// # }
/// # async fn send_out_sync_request(client: &Client) -> Result<()> {
/// #    unimplemented!();
/// # }
/// async fn sync(client: &Client) -> Result<()> {
///     // This is happening at the top of the method so we advertise our
///     // end-to-end encryption capabilities as soon as possible.
///     process_outgoing_requests(client).await?;
///
///     // We can sync with the homeserver now.
///     let response = send_out_sync_request(client).await?;
///
///     // Process the sync response here.
///
///     Ok(())
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## Receiving room keys and related changes
///
/// The next step in our implementation is to forward messages that were sent
/// directly to the client's device, and state updates about the one-time
/// prekeys, to the [`OlmMachine`]. This is achieved using
/// the [`OlmMachine::receive_sync_changes()`] method.
///
/// The method performs two tasks:
///
/// 1. It processes and, if necessary, decrypts each [to-device] event that was
///    pushed into it, and returns the decrypted events. The original events are
///    replaced with their decrypted versions.
///
/// 2. It produces internal state changes that may trigger the creation of new
///    outgoing requests. For example, if the server informs the client that its
///    one-time prekeys have been depleted, the OlmMachine will create an
///    outgoing request to replenish them.
///
/// Our updated sync method now looks like this:
///
/// ```no_run
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::{
///     DecryptionSettings, EncryptionSyncChanges, OlmMachine, TrustRequirement
/// };
/// # use ruma::api::client::sync::sync_events::v3::Response;
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # struct Client {
/// #     outgoing_requests_lock: tokio::sync::Mutex<()>,
/// #     olm_machine: OlmMachine,
/// # }
/// # async fn process_outgoing_requests(client: &Client) -> Result<()> {
/// #    unimplemented!();
/// # }
/// # async fn send_out_sync_request(client: &Client) -> Result<Response> {
/// #    unimplemented!();
/// # }
/// async fn sync(client: &Client) -> Result<()> {
///     process_outgoing_requests(client).await?;
///
///     let response = send_out_sync_request(client).await?;
///
///     let sync_changes = EncryptionSyncChanges {
///         to_device_events: response.to_device.events,
///         changed_devices: &response.device_lists,
///         one_time_keys_counts: &response.device_one_time_keys_count,
///         unused_fallback_keys: response.device_unused_fallback_key_types.as_deref(),
///         next_batch_token: Some(response.next_batch),
///     };
///
///     let decryption_settings = DecryptionSettings {
///         sender_device_trust_requirement: TrustRequirement::Untrusted
///     };
///
///     // Push the sync changes into the OlmMachine, make sure that this is
///     // happening before the `next_batch` token of the sync is persisted.
///     let to_device_events = client
///         .olm_machine
///         .receive_sync_changes(sync_changes, &decryption_settings)
///         .await?;
///
///     // Send the outgoing requests out that the sync changes produced.
///     process_outgoing_requests(client).await?;
///
///     // Process the rest of the sync response here.
///
///     Ok(())
/// }
/// # Ok(())
/// # }
/// ```
///
/// It is important to note that the names of the fields in the response shown
/// in the example match the names of the fields specified in the [sync]
/// response specification.
///
/// It is critical to note that due to the ephemeral nature of to-device
/// events[[1]], it is important to process these events before persisting the
/// `next_batch` sync token. This is because if the `next_batch` sync token is
/// persisted before processing the to-device events, some messages might be
/// lost, leading to decryption failures.
///
/// ## Decrypting room events
///
/// The final step in the decryption process is to decrypt the room events that
/// are received from the server. To do this, the encrypted events must be
/// passed to the [`OlmMachine`], which will use the keys that were previously
/// exchanged between devices to decrypt the events. The decrypted events can
/// then be processed and displayed to the user in the Matrix client.
///
/// Room message [events] can be decrypted using the
/// [`OlmMachine::decrypt_room_event()`] method:
///
/// ```no_run
/// # use std::collections::BTreeMap;
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::{OlmMachine, DecryptionSettings, TrustRequirement};
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let encrypted = unimplemented!();
/// # let room_id = unimplemented!();
/// # let machine: OlmMachine = unimplemented!();
/// # let settings = DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
/// // Decrypt your room events now.
/// let decrypted = machine
///     .decrypt_room_event(encrypted, room_id, &settings)
///     .await?;
/// # Ok(())
/// # }
/// ```
/// It's worth mentioning that the [`OlmMachine::decrypt_room_event()`] method
/// is designed to be thread-safe and can be safely called concurrently. This
/// means that room message [events] can be processed in parallel, improving the
/// overall efficiency of the end-to-end encryption implementation.
///
/// By allowing room message [events] to be processed concurrently, the client's
/// implementation can take full advantage of the capabilities of modern
/// hardware and achieve better performance, especially when dealing with a
/// large number of messages at once.
///
/// # Encryption
///
/// In this section of the guide, we will focus on enabling the encryption of
/// messages in our Matrix client library. Up until this point, we have been
/// discussing the process of decrypting messages that have been encrypted by
/// other devices. Now, we will shift our focus to the process of encrypting
/// messages on the client side, so that they can be securely transmitted over
/// the Matrix network to other devices.
///
/// This section will guide you through the steps required to set up the
/// encryption process, including establishing the necessary sessions and
/// encrypting messages using the Megolm group session. The specific steps are
/// outlined below:
///
/// 1. [Cryptographic devices of other users need to be
///    discovered](#tracking-users)
///
/// 2. [Secure channels between the devices need to be
///    established](#establishing-end-to-end-encrypted-channels)
///
/// 3. [A room key needs to be exchanged with the group](#exchanging-room-keys)
///
/// 4. [Individual messages need to be encrypted using the room
///    key](#encrypting-room-events)
///
/// The process for enabling encryption in a two-device scenario is also
/// depicted in the following sequence diagram:
///
/// ```mermaid
/// sequenceDiagram
/// actor Alice
/// participant Homeserver
/// actor Bob
///
/// Alice->>Homeserver: Download Bob's one-time prekey
/// Homeserver->>Alice: Bob's one-time prekey
/// Alice->>Alice: Encrypt the room key
/// Alice->>Homeserver: Send the room key to each of Bob's devices
/// Homeserver->>Bob: Deliver the room key
/// Alice->>Alice: Encrypt the message
/// Alice->>Homeserver: Send the encrypted message
/// Homeserver->>Bob: Deliver the encrypted message
/// ```
///
/// In the following subsections, we will provide a step-by-step guide on how to
/// enable the encryption of messages using the OlmMachine. We will outline the
/// specific method calls and usage patterns that are required to establish the
/// necessary sessions, encrypt messages, and send them over the Matrix network.
///
/// ## Tracking users
///
/// The first step in the process of encrypting a message and sending it to a
/// device is to discover the devices that the recipient user has. This can be
/// achieved by sending a request to the homeserver to retrieve a list of the
/// recipient's device keys. The response to this request will include the
/// device keys for all of the devices that belong to the recipient, as well as
/// information about their current status and whether or not they support
/// end-to-end encryption.
///
/// The process for discovering and keeping track of devices for a user is
/// outlined in the Matrix specification in the "[Tracking the device list for a
/// user]" section.
///
/// A simplified sequence diagram of the process can also be found below.
///
/// ```mermaid
/// sequenceDiagram
/// actor Alice
/// participant Homeserver
///
/// Alice->>Homeserver: Sync with the homeserver
/// Homeserver->>Alice: Users whose device list has changed
/// Alice->>Alice: Mark user's devicel list as outdated
/// Alice->>Homeserver: Ask the server for the new device list of all the outdated users
/// Alice->>Alice: Update the local device list and mark the users as up-to-date
/// ```
///
/// The OlmMachine refers to users whose devices we are tracking as "tracked
/// users" and utilizes the [`OlmMachine::update_tracked_users()`] method to
/// start considering users to be tracked. Keeping the above diagram in mind, we
/// can now update our sync method as follows:
///
/// ```no_run
/// # use anyhow::Result;
/// # use std::ops::Deref;
/// # use matrix_sdk_crypto::{
/// #     DecryptionSettings, EncryptionSyncChanges, OlmMachine, TrustRequirement
/// # };
/// # use ruma::api::client::sync::sync_events::v3::{Response, State, JoinedRoom};
/// # use ruma::{OwnedUserId, serde::Raw, events::AnySyncStateEvent};
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # struct Client {
/// #     outgoing_requests_lock: tokio::sync::Mutex<()>,
/// #     olm_machine: OlmMachine,
/// # }
/// # async fn process_outgoing_requests(client: &Client) -> Result<()> {
/// #    unimplemented!();
/// # }
/// # async fn send_out_sync_request(client: &Client) -> Result<Response> {
/// #    unimplemented!();
/// # }
/// # fn is_member_event_of_a_joined_user(event: &Raw<AnySyncStateEvent>) -> bool {
/// #     true
/// # }
/// # fn get_user_id(event: &Raw<AnySyncStateEvent>) -> OwnedUserId {
/// #     unimplemented!();
/// # }
/// # fn is_room_encrypted(room: &JoinedRoom) -> bool {
/// #     true
/// # }
/// async fn sync(client: &Client) -> Result<()> {
///     process_outgoing_requests(client).await?;
///
///     let response = send_out_sync_request(client).await?;
///
///     let sync_changes = EncryptionSyncChanges {
///         to_device_events: response.to_device.events,
///         changed_devices: &response.device_lists,
///         one_time_keys_counts: &response.device_one_time_keys_count,
///         unused_fallback_keys: response.device_unused_fallback_key_types.as_deref(),
///         next_batch_token: Some(response.next_batch),
///     };
///
///     let decryption_settings = DecryptionSettings {
///         sender_device_trust_requirement: TrustRequirement::Untrusted
///     };
///
///     // Push the sync changes into the OlmMachine, make sure that this is
///     // happening before the `next_batch` token of the sync is persisted.
///     let to_device_events = client
///         .olm_machine
///         .receive_sync_changes(sync_changes, &decryption_settings)
///         .await?;
///
///     // Send the outgoing requests out that the sync changes produced.
///     process_outgoing_requests(client).await?;
///
///     // Collect all the joined and invited users of our end-to-end encrypted rooms here.
///     let mut users = Vec::new();
///
///     for (_, room) in &response.rooms.join {
///         // For simplicity reasons we're only looking at the state field of a joined room, but
///         // the events in the timeline are important as well.
///         if let State::Before(state) = &room.state {
///            for event in &state.events {
///                 if is_member_event_of_a_joined_user(event) && is_room_encrypted(room) {
///                     let user_id = get_user_id(event);
///                     users.push(user_id);
///                 }
///             }
///         }
///     }
///
///     // Mark all the users that we consider to be in a end-to-end encrypted room with us to be
///     // tracked. We need to know about all the devices each user has so we can later encrypt
///     // messages for each of their devices.
///     client.olm_machine.update_tracked_users(users.iter().map(Deref::deref)).await?;
///
///     // Process the rest of the sync response here.
///
///     Ok(())
/// }
/// # Ok(())
/// # }
/// ```
///
/// Now that we have discovered the devices of the users we'd like to
/// communicate with in an end-to-end encrypted manner, we can start considering
/// encrypting messages for those devices. This concludes the sync processing
/// method, we are now ready to move on to the next section, which will explain
/// how to begin the encryption process.
///
/// ## Establishing end-to-end encrypted channels
///
/// In the [Triple
/// Diffie-Hellman](#using-the-triple-diffie-hellman-key-agreement-protocol)
/// section, we described the need for two Curve25519 keys from the recipient
/// device to establish a 1-to-1 secure channel: the long-term identity key of a
/// device and a one-time prekey. In the previous section, we started tracking
/// the device keys, including the long-term identity key that we need. The next
/// step is to download the one-time prekey on an on-demand basis and establish
/// the 1-to-1 secure channel.
///
/// To accomplish this, we can use the [`OlmMachine::get_missing_sessions()`]
/// method in bulk, which will claim the one-time prekey for all the devices of
/// a user that we're not already sharing a 1-to-1 encrypted channel with.
///
/// #### 🔒 Locking rule
///
/// As with the [`OlmMachine::outgoing_requests()`] method, it is necessary to
/// protect this method with a lock, otherwise we will be creating more 1-to-1
/// encrypted channels than necessary.
///
/// ```no_run
/// # use std::collections::{BTreeMap, HashSet};
/// # use std::ops::Deref;
/// # use anyhow::Result;
/// # use ruma::UserId;
/// # use ruma::api::client::keys::claim_keys::v3::{Response, Request};
/// # use matrix_sdk_crypto::OlmMachine;
/// # async fn send_request(request: &Request) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let users: HashSet<&UserId> = HashSet::new();
/// # let machine: OlmMachine = unimplemented!();
/// // Mark all the users that are part of an encrypted room as tracked
/// if let Some((request_id, request)) =
///     machine.get_missing_sessions(users.iter().map(Deref::deref)).await?
/// {
///     let response = send_request(&request).await?;
///     machine.mark_request_as_sent(&request_id, &response).await?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// With the ability to exchange messages directly with devices, we can now
/// start sharing room keys over the 1-to-1 encrypted channel.
///
/// ## Exchanging room keys
///
/// To exchange a room key with our group, we will once again take a bulk
/// approach. The [`OlmMachine::share_room_key()`] method is used to accomplish
/// this step. This method will create a new room key, if necessary, and encrypt
/// it for each device belonging to the users provided as an argument. It will
/// then output an array of sendToDevice requests that we must send to the
/// server, and mark the requests as sent.
///
/// #### 🔒 Locking rule
///
/// Like some of the previous methods, OlmMachine::share_room_key() needs to be
/// protected by a lock to prevent the possibility of creating and sending
/// multiple room keys simultaneously for the same group. The lock can be
/// implemented on a per-room basis, which allows for parallel room key
/// exchanges across different rooms.
///
/// ```no_run
/// # use std::collections::{BTreeMap, HashSet};
/// # use std::ops::Deref;
/// # use anyhow::Result;
/// # use ruma::UserId;
/// # use ruma::api::client::keys::claim_keys::v3::{Response, Request};
/// # use matrix_sdk_crypto::{OlmMachine, types::requests::ToDeviceRequest, EncryptionSettings};
/// # async fn send_request(request: &ToDeviceRequest) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let users: HashSet<&UserId> = HashSet::new();
/// # let room_id = unimplemented!();
/// # let settings = EncryptionSettings::default();
/// # let machine: OlmMachine = unimplemented!();
/// // Let's share a room key with our group.
/// let requests = machine.share_room_key(
///     room_id,
///     users.iter().map(Deref::deref),
///     EncryptionSettings::default(),
/// ).await?;
///
/// // Make sure each request is sent out
/// for request in requests {
///     let request_id = &request.txn_id;
///     let response = send_request(&request).await?;
///     machine.mark_request_as_sent(&request_id, &response).await?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// In order to ensure that room keys are rotated and exchanged when needed, the
/// [`OlmMachine::share_room_key()`] method should be called before sending
/// each room message in an end-to-end encrypted room. If a room key has
/// already been exchanged, the method becomes a no-op.
///
/// ## Encrypting room events
///
/// After the room key has been successfully shared, a plaintext can be
/// encrypted.
///
/// ```no_run
/// # use anyhow::Result;
/// # use matrix_sdk_crypto::{DecryptionSettings, OlmMachine, TrustRequirement};
/// # use ruma::events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent};
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let room_id = unimplemented!();
/// # let event = unimplemented!();
/// # let machine: OlmMachine = unimplemented!();
/// # let settings = DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
/// let content = AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain("It's a secret to everybody."));
/// let encrypted_content = machine.encrypt_room_event(room_id, content).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Appendix: Combining the session creation and room key exchange
///
/// The steps from the previous three sections should combined into a single
/// method that is used to send messages.
///
/// ```no_run
/// # use std::collections::{BTreeMap, HashSet};
/// # use std::ops::Deref;
/// # use anyhow::Result;
/// # use serde_json::json;
/// # use ruma::{UserId, RoomId, serde::Raw};
/// # use ruma::api::client::keys::claim_keys::v3::{Response, Request};
/// # use matrix_sdk_crypto::{EncryptionSettings, OlmMachine, types::requests::ToDeviceRequest};
/// # use tokio::sync::MutexGuard;
/// # async fn send_request(request: &Request) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # async fn send_to_device_request(request: &ToDeviceRequest) -> Result<Response> {
/// #     let response = unimplemented!();
/// #     Ok(response)
/// # }
/// # async fn acquire_per_room_lock(room_id: &RoomId) -> MutexGuard<()> {
/// #     unimplemented!();
/// # }
/// # async fn get_joined_members(room_id: &RoomId) -> Vec<&UserId> {
/// #    unimplemented!();
/// # }
/// # fn is_room_encrypted(room_id: &RoomId) -> bool {
/// #     true
/// # }
/// # #[tokio::main]
/// # async fn main() -> Result<()> {
/// # let users: HashSet<&UserId> = HashSet::new();
/// # let machine: OlmMachine = unimplemented!();
/// struct Client {
///     session_establishment_lock: tokio::sync::Mutex<()>,
///     olm_machine: OlmMachine,
/// }
///
/// async fn establish_sessions(client: &Client, users: &[&UserId]) -> Result<()> {
///     if let Some((request_id, request)) =
///         client.olm_machine.get_missing_sessions(users.iter().map(Deref::deref)).await?
///     {
///         let response = send_request(&request).await?;
///         client.olm_machine.mark_request_as_sent(&request_id, &response).await?;
///     }
///
///     Ok(())
/// }
///
/// async fn share_room_key(machine: &OlmMachine, room_id: &RoomId, users: &[&UserId]) -> Result<()> {
///     let _lock = acquire_per_room_lock(room_id).await;
///
///     let requests = machine.share_room_key(
///             room_id,
///             users.iter().map(Deref::deref),
///             EncryptionSettings::default(),
///     ).await?;
///
///     // Make sure each request is sent out
///     for request in requests {
///         let request_id = &request.txn_id;
///         let response = send_to_device_request(&request).await?;
///         machine.mark_request_as_sent(&request_id, &response).await?;
///     }
///
///     Ok(())
/// }
///
/// async fn send_message(client: &Client, room_id: &RoomId, message: &str) -> Result<()> {
///     let mut content = json!({
///         "body": message,
///             "msgtype": "m.text",
///     });
///
///     if is_room_encrypted(room_id) {
///         let content = Raw::new(&json!({
///             "body": message,
///             "msgtype": "m.text",
///         }))?.cast_unchecked();
///
///         let users = get_joined_members(room_id).await;
///
///         establish_sessions(client, &users).await?;
///         share_room_key(&client.olm_machine, room_id, &users).await?;
///
///         let encrypted = client
///             .olm_machine
///             .encrypt_room_event_raw(room_id, "m.room.message", &content)
///             .await?;
///     }
///
///     Ok(())
/// }
/// # Ok(())
/// # }
/// ```
///
/// TODO
///
/// [Matrix]: https://matrix.org/
/// [Olm]: https://gitlab.matrix.org/matrix-org/olm/-/blob/master/docs/olm.md
/// [Diffie-Hellman]: https://en.wikipedia.org/wiki/Diffie%E2%80%93Hellman_key_exchange
/// [Megolm]: https://gitlab.matrix.org/matrix-org/olm/blob/master/docs/megolm.md
/// [end-to-end-encryption]: https://en.wikipedia.org/wiki/End-to-end_encryption
/// [homeserver]: https://spec.matrix.org/unstable/#architecture
/// [key-agreement protocol]: https://en.wikipedia.org/wiki/Key-agreement_protocol
/// [client-server specification]: https://matrix.org/docs/spec/client_server/
/// [forward secrecy]: https://en.wikipedia.org/wiki/Forward_secrecy
/// [replay attacks]: https://en.wikipedia.org/wiki/Replay_attack
/// [Tracking the device list for a user]: https://spec.matrix.org/unstable/client-server-api/#tracking-the-device-list-for-a-user
/// [X3DH]: https://signal.org/docs/specifications/x3dh/
/// [to-device]: https://spec.matrix.org/unstable/client-server-api/#send-to-device-messaging
/// [sync]: https://spec.matrix.org/unstable/client-server-api/#get_matrixclientv3sync
/// [events]: https://spec.matrix.org/unstable/client-server-api/#events
///
/// [1]: https://spec.matrix.org/unstable/client-server-api/#server-behaviour-4
pub mod tutorial {}

#[cfg(test)]
mod test {
    use insta::assert_json_snapshot;

    use crate::{DecryptionSettings, TrustRequirement};

    #[test]
    fn snapshot_trust_requirement() {
        assert_json_snapshot!(TrustRequirement::Untrusted);
        assert_json_snapshot!(TrustRequirement::CrossSignedOrLegacy);
        assert_json_snapshot!(TrustRequirement::CrossSigned);
    }

    #[test]
    fn snapshot_decryption_settings() {
        assert_json_snapshot!(DecryptionSettings {
            sender_device_trust_requirement: TrustRequirement::Untrusted,
        });
    }
}
