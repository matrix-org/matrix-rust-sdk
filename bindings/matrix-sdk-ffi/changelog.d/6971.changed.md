`SessionVerificationController` no longer surfaces an incoming verification
request this session can't complete. A verified session that is missing the
private self-signing key can neither sign the other device nor be signed by it,
so the request is dropped instead of being offered to the user only to fail
after the emojis have been compared. Requests received while this session is
still unverified are unaffected: the other side is then the one signing us.
