# unreleased

- `save_change` performance improvement, all encryption and serialization
  is done now outside of the db transaction.