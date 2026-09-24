When a room key arrives from a device we don't know about, mark the sender's
device list as outdated so it gets queried again. Once the device is known, the
session's sender data is updated and events encrypted with it can be decrypted.
Previously, a stale device list left these events undecryptable with "The sending
device is not known" under the `CrossSignedOrLegacy` trust requirement.
