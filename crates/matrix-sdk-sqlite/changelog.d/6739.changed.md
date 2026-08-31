The `EventId`s stored in the Event Cache database are now encrypted as hashes,
they cannot be decrypted.

The gap `prev_batch_token` is also encoded from a Rust `String` instead of a
JSON representation of a `String`, saving a bit of computation and storage
space.

The Event Cache database is reset.
