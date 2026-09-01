A user identity whose master key carries a valid X.509 signature
chaining to one of the configured trust anchors now confers trust
on that user's devices.

Previously, such an identity was reported as verified by
`UserIdentity::is_verified()`, but its devices would still be
reported as unverified by `Device::is_verified()`, and the room key
sharing strategies withheld room keys from them.

Requires the `experimental-x509-identity-verification` feature and a
configured X.509 verifier.
