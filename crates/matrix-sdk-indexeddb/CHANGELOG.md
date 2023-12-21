# unreleased

- `save_change` performance improvement, all ecnryption and serialization
  is done now outside of the db transaction.