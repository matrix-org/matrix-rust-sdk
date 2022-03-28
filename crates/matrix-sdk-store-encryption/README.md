A general purpose encryption scheme for key/value stores.

# Usage

```rust
use matrix_sdk_store_encryption::StoreCipher;
use serde_json::{json, value::Value};

fn main() -> anyhow::Result<()> {
    let store_cipher = StoreCipher::new()?;

    // Export the store cipher and persist it in your key/value store
    let export = store_cipher.export("secret-passphrase")?;

    let value = json!({
        "some": "data",
    });

    let encrypted = store_cipher.encrypt_value(&value)?;
    let decrypted: Value = store_cipher.decrypt_value(&encrypted)?;

    assert_eq!(value, decrypted);

    let key = "bulbasaur";

    // Hash the key so people don't know which pokemon we have collected.
    let hashed_key = store_cipher.hash_key("list-of-pokemon", key.as_ref());
    let another_table = store_cipher.hash_key("my-starter", key.as_ref());
    let same_key = store_cipher.hash_key("my-starter", key.as_ref());

    assert_ne!(key.as_ref(), hashed_key);
    assert_ne!(hashed_key, another_table);
    assert_eq!(another_table, same_key);

    Ok(())
}
```

# ⚠️ Security Warning: Hazmat!

This crate only implements the low-level block cipher function, to be used
*only* as a building block for higher-level constructions. It is NOT intended
for direct use in applications.

USE AT YOUR OWN RISK!

# Encryption scheme

The central component of the encryption scheme is the `StoreCipher` type, used
for both obfuscating keys and encrypting values of the key/value store.
A `StoreCipher` object consists of two randomly-generated 32 byte secrets.

The first secret is used to encrypt values. XChaCha20Poly1305 with a random
nonce is used to encrypt each value. The nonce is saved with the ciphertext.

The second secret is used as a seed to derive table-specific keys, used to key
a keyed hash construction, which is in turn used to hash table data. Currently
we use blake3 as the keyed hash construction.

```text
                ┌───────────────────────────────────────┐
                │             StoreCipher               │
                │   Encryption key |    Hash key seed   │
                │      [u8; 32]    |       [u8; 32]     │
                └───────────────────────────────────────┘
```

The `StoreCipher` has some Matrix-specific assumptions built in, which ensure that
the limits of the cryptographic primitives are not exceeded. If this crate is
used for non-Matrix data, users need to ensure:

1. That individual values are chunked, otherwise decryption might be susceptible
   to a DOS attack.
2. The `StoreCipher` is periodically rotated/rekeyed.

# WASM support

This crate relies on the `random` and `getrandom` crates which don't support
WASM automatically.

Either turn the `js` feature on directly on this crate or depend on `getrandom`
with the `js` feature turned on. More info can be found in the [`getrandom`
docs](https://docs.rs/getrandom/latest/getrandom/index.html#webassembly-support).
