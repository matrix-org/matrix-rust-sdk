A general purpose encryption scheme for key/value stores.

# Usage

```rust
use matrix_sdk_store_key::StoreKey;
use serde_json::{json, value::Value};

fn main() -> anyhow::Result<()> {
    let store_key = StoreKey::new()?;

    // Export the store key and persist it in your key/value store
    let export = store_key.export("secret-passphrase")?;

    let value = json!({
        "some": "data",
    });

    let encrypted = store_key.encrypt_value(&value)?;
    let decrypted: Value = store_key.decrypt_value(&encrypted)?;

    assert_eq!(value, decrypted);

    let key = "bulbasaur";

    // Hash the key so people don't know which pokemon we have collected.
    let hashed_key = store_key.hash_key("list-of-pokemon", key.as_ref());
    let another_table = store_key.hash_key("my-starter", key.as_ref());
    let same_key = store_key.hash_key("my-starter", key.as_ref());

    assert_ne!(key.as_ref(), hashed_key);
    assert_ne!(hashed_key, another_table);
    assert_eq!(another_table, same_key);

    Ok(())
}
```

# ⚠️ Security Warning: Hazmat!

This crate implements only the low-level block cipher function, and is intended
for use for implementing higher-level constructions *only*. It is NOT
intended for direct use in applications.

USE AT YOUR OWN RISK!

# Encryption scheme

A `StoreKey` consists of two randomly generated 32 byte-sized slices.

The first 32 bytes are used to encrypt values. XChaCha20Poly1305 with a random
nonce is used to encrypt each value. The nonce is saved with the ciphertext.

The second 32 bytes are used as a seed to derive table specific keys that will
be used to hash data. The data will then be hashed using the blake3 keyed hash
using the table specific key.

```text
                ┌───────────────────────────────────────┐
                │               StoreKey                │
                │   Encryption key |    Hash key seed   │
                │      [u8; 32]    |       [u8; 32]     │
                └───────────────────────────────────────┘
```

The `StoreKey` has some Matrix specific assumptions built in which ensure that
the limits of the cryptographic primitives are not exceeded. If this crate is
used for non-Matrix specific data users need to ensure:

1. That individual values are chunked, otherwise decryption might be succeptible
   to a DOS attack.
2. The `StoreKey` is periodically rotated/rekeyed.
