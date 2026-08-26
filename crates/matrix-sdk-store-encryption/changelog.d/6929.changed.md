**breaking** The `StoreCipher::export_with_key` and
`StoreCipher::import_with_key` now accept a variable sized key. The functions
derive a 32-byte sub-key to encrypt and decrypt the `StoreCipher`. Importing
existing exports which didn't use the sub-key mechanism will continue working.
