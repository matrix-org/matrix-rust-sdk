Introduces `Client::set_x509_signer` and `Client::set_x509_verifier`,
which are passed to the client's `OlmMachine` to enable experimental
support for X.509-based cross-signing identity verification.

Gated behind the `experimental-x509-identity-verification` feature.
