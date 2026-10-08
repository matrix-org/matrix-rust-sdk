Add `OAuth::login_with_device_code()` to log in with the OAuth 2.0 Device
Authorization Grant ([RFC 8628](https://datatracker.ietf.org/doc/html/rfc8628)),
for clients that cannot open a browser such as bots, bridges or command-line
applications. The returned `LoginWithDeviceCode` future exposes the verification
URI and user code to show to the user through `subscribe_to_progress()`, then
waits for the user's approval and loads the session like other OAuth 2.0 logins.
It does not require the `e2e-encryption` feature. Consequently,
`DeviceAuthorizationOAuthError` moved to `authentication::oauth::error` and is
still re-exported from `authentication::oauth::qrcode`, and the new
`DeviceCodeLoginError` describes the failures of this login method.
