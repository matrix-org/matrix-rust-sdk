**matrix-sdk-qrcode** is a crate to easily generate and parse QR codes for
interactive verification using [QR codes] in Matrix.

## Usage

This is probably not the crate you are looking for, it's used internally in the
[matrix-sdk].

If you still want to play with QR codes, here is a helpful example.

### Encode into a QR code

```rust,no_run
use matrix_sdk_qrcode::{QrVerificationData, DecodingError};
use image::Luma;

fn main() -> Result<(), DecodingError> {
    let data = b"MATRIX\
        \x02\x02\x00\x07\
        FLOW_ID\
        AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
        BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
        SHARED_SECRET";

    let data = QrVerificationData::from_bytes(data)?;
    let encoded = data.to_qr_code().unwrap();
    let image = encoded.render::<Luma<u8>>().build();

    Ok(())
}
```

[matrix-sdk]: https://github.com/matrix-org/matrix-rust-sdk/
[QR codes]: https://spec.matrix.org/unstable/client-server-api/#qr-codes
