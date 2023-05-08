End-to-end encryption related types

Matrix has support for end-to-end encrypted messaging, this module contains
types related to end-to-end encryption, describes a bit how E2EE works in
the matrix-sdk, and how to set your [`Client`] up to support E2EE.

Jump to the [Client Setup](#client-setup) section if you don't care how E2EE
works under the hood.

# End-to-end encryption

While all messages in Matrix land are transferred to the server in an
encrypted manner, rooms can be marked as end-to-end encrypted. If a room is
marked as end-to-end encrypted, using a `m.room.encrypted` state event, all
messages that are sent to this room will be encrypted for the individual
room members. This means that the server won't be able to read messages that
get sent to such a room.

```text
                              ┌──────────────┐
                              │  Homeserver  │
     ┌───────┐                │              │                ┌───────┐
     │ Alice │═══════════════►│  unencrypted │═══════════════►│  Bob  │
     └───────┘   encrypted    │              │   encrypted    └───────┘
                              └──────────────┘
```

```text
                              ┌──────────────┐
                              │  Homeserver  │
     ┌───────┐                │              │                ┌───────┐
     │ Alice │≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡►│─────────────►│≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡►│  Bob  │
     └───────┘   encrypted    │   encrypted  │   encrypted    └───────┘
                              └──────────────┘
```

## Encrypting for each end

We already mentioned that a message in a end-to-end encrypted world needs to
be encrypted for each individual member, though that isn't completely
correct. A message needs to be encrypted for each individual *end*. An *end*
in Matrix land is a client that communicates with the homeserver. The spec
calls an *end* a Device, while other clients might call an *end* a Session.

The matrix-sdk represents an *end* as a [`Device`] object. Each individual
message should be encrypted for each individual [`Device`] of each
individual room member.

Since rooms might grow quite big, encrypting each message for every
[`Device`] becomes quickly unsustainable. Because of that room keys have
been introduced.

## Room keys

Room keys remove the need to encrypt each message for each *end*.
Instead a room key needs to be shared with each *end*, after that a message
can be encrypted in a single, O(1), step.

A room key is backed by a [Megolm] session, which in turn consists two
parts. The first part, the outbound group session is used for encryption,
this one never leaves your device. The second part is the inbound group
session, which is shared with each *end*.

```text
            ┌────────────────────────┬───────────────────────┐
            │       Encryption       │      Decryption       │
            ├────────────────────────┼───────────────────────┤
            │ Outbound group session │ Inbound group session │
            └────────────────────────┴───────────────────────┘
```

### Lifetime of a room key

1. Create a room key
2. Share the room key with each participant
3. Encrypt messages using the room key
4. If needed, rotate the room key and go back to 1

The `m.room.encryption` state event of the room decides how long a room key
should be used. By default this is for 100 messages or for 1 week, whichever
comes first.

### Decrypting the room history

Since room keys get relatively often rotated, each room key will need to be
stored, otherwise we won't be able to decrypt historical messages. The SDK
stores all room keys locally in an encrypted manner.

Besides storing them as part of the SDK store, users can export room keys
using the [`Encryption::export_room_keys`] method.

# Verification

One important aspect of end-to-end encryption is to check that the *end* you
are communicating with is indeed the person you expect. This checking is
done in Matrix via interactive verification. While interactively verifying,
we'll need to exchange some critical piece of information over another
communication channel, over the phone, or in person are good candidates
for such a channel.

Usually each *end* will need to verify every *end* it communicates with. An
*end* is represented as a [`Device`] in the matrix-sdk. This gets rather
complicated quickly as is shown bellow, with Alice and Bob each having two
devices. Each arrow represents who needs to verify whom for the
communication between Alice and Bob to be considered secure.

```text

              ┌───────────────────────────────────────────┐
              ▼                                           │
        ┌───────────┐                                ┌────┴────┐
      ┌►│Alice Phone├───────────────────────────────►│Bob Phone│◄──┐
      │ └─────┬─────┘                                └─────┬───┘   │
      │       ▼                                            ▼       │
      │ ┌────────────┐                               ┌───────────┐ │
      └─┤Alice Laptop├──────────────────────────────►│Bob Desktop├─┘
        └────────────┘                               └─────┬─────┘
              ▲                                            │
              └────────────────────────────────────────────┘

```

To simplify things and lower the amount of devices a user needs to verify
cross signing has been introduced. Cross signing adds a concept of a user
identity which is represented in the matrix-sdk using the [`UserIdentity`]
struct. This way Alice and Bob only need to verify their own devices and
each others user identity for the communication to be considered secure.

```text

           ┌─────────────────────────────────────────────────┐
           │   ┌─────────────────────────────────────────┐   │
           ▼   │                                         ▼   │
    ┌──────────┴─────────┐                   ┌───────────────┴──────┐
    │┌──────────────────┐│                   │  ┌────────────────┐  │
    ││Alice UserIdentity││                   │  │Bob UserIdentity│  │
    │└───┬─────────┬────┘│                   │  └─┬───────────┬──┘  │
    │    │         │     │                   │    │           │     │
    │    ▼         ▼     │                   │    ▼           ▼     │
    │┌───────┐ ┌────────┐│                   │┌───────┐  ┌─────────┐│
    ││ Alice │ │ Alice  ││                   ││  Bob  │  │   Bob   ││
    ││ Phone │ │ Laptop ││                   ││ Phone │  │ Desktop ││
    │└───────┘ └────────┘│                   │└───────┘  └─────────┘│
    └────────────────────┘                   └──────────────────────┘

```

More info about devices and identities can be found in the [`identities`]
module.

To add interactive verification support to your client please see the
[`verification`] module, also check out the documentation for the
[`Device::is_verified()`] method, which explains in more detail what
it means for a [`Device`] to be verified.

# Client setup

The matrix-sdk aims to provide encryption support transparently. If
encryption is enabled and correctly set up, events that need to be encrypted
will be encrypted automatically. Events will also be decrypted
automatically.

Please note that, unless a client is specifically set up to ignore
unverified devices, verifying devices is **not** necessary for encryption
to work.

1. Make sure the `e2e-encryption` feature is enabled.
2. To persist the encryption keys, you can use [`ClientBuilder::store_config`]
   or one of the other `_store` methods on [`ClientBuilder`].

## Restoring a client

Restoring a Client is relatively easy, still some things need to be kept in
mind before doing so.

There are two ways one might wish to restore a [`Client`]:

1. Using an access token
2. Using the password

Initially, logging in creates a device ID and access token on the server,
those two are directly connected to each other, more on this relationship
can be found in the [spec].

After we log in the client will upload the end-to-end encryption related
[device keys] to the server. Those device keys cannot be replaced once they
have been uploaded and tied to a device ID.

### Using an access token

1. Log in with the password using [`Client::login_username()`].
2. Store the access token, preferably somewhere secure.
3. Use [`Client::restore_session()`] the next time the client starts.

**Note** that the access token is directly connected to a device ID that
lives on a server. If you're skipping step one of this method, remember that
you **can't** use an access token that already has some device keys tied to
the device ID.

### Using a password.

1. Log in using [`Client::login_username()`].
2. Store the `device_id` that was returned in the login response from the
   server.
3. Use [`Client::login_username()`] the next time the client starts, make sure
   to set `device_id` to the stored `device_id` from the previous step. This
   will replace the access token from the previous login call, but won't create
   a new device.

**Note** that the default store supports only a single device, logging in
with a different device ID (either `None` or a device ID of another client)
is **not** supported using the default store.

## Common pitfalls

| Failure | Cause | Fix |
| ------------------- | ----- | ----------- |
| No messages get encrypted nor decrypted | The `e2e-encryption` feature is disabled | [Enable the feature in your `Cargo.toml` file] |
| Messages that were decryptable aren't after a restart | Storage isn't setup to be persistent | Ensure you've activated the persistent storage backend feature, e.g. `sqlite` |
| Messages are encrypted but can't be decrypted | The access token that the client is using is tied to another device | Clear storage to create a new device, read the [Restoring a Client] section |
| Messages don't get encrypted but get decrypted | The `m.room.encryption` event is missing | Make sure encryption is [enabled] for the room and the event isn't [filtered] out, otherwise it might be a deserialization bug |

[Enable the feature in your `Cargo.toml` file]: https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#choosing-features
[Megolm]: https://gitlab.matrix.org/matrix-org/olm/blob/master/docs/megolm.md
[`UserIdentity`]: #struct.verification.UserIdentity
[filtered]: crate::config::SyncSettings::filter
[enabled]: crate::room::Joined::enable_encryption
[Restoring a Client]: #restoring-a-client
[spec]: https://spec.matrix.org/unstable/client-server-api/#relationship-between-access-tokens-and-devices
[device keys]: https://spec.matrix.org/unstable/client-server-api/#device-keys
[`store`]: crate::store
[`CryptoStore`]: matrix_sdk_base::crypto::store::CryptoStore
[`StoreConfig`]: crate::config::StoreConfig
[`ClientBuilder`]: crate::ClientBuilder
[`ClientBuilder::store_config`]: crate::ClientBuilder::store_config
