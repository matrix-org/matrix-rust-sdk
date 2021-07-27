[![Build Status](https://img.shields.io/travis/matrix-org/matrix-rust-sdk.svg?style=flat-square)](https://travis-ci.org/matrix-org/matrix-rust-sdk)
[![codecov](https://img.shields.io/codecov/c/github/matrix-org/matrix-rust-sdk/master.svg?style=flat-square)](https://codecov.io/gh/matrix-org/matrix-rust-sdk)
[![License](https://img.shields.io/badge/License-Apache%202.0-yellowgreen.svg?style=flat-square)](https://opensource.org/licenses/Apache-2.0)
[![#matrix-rust-sdk](https://img.shields.io/badge/matrix-%23matrix--rust--sdk-blue?style=flat-square)](https://matrix.to/#/#matrix-rust-sdk:matrix.org)

# matrix-qrcode

**matrix-qrcode** is a crate to easily generate and parse QR codes for
interactive verification using [QR codes] in Matrix.

[Matrix]: https://matrix.org/
[Rust]: https://www.rust-lang.org/
[QR codes]: https://spec.matrix.org/unstable/client-server-api/#qr-codes

## Usage

This is probably not the crate you are looking for, it's used internally in the
matrix-rust-sdk.

If you still want to play with QR codes, here are a couple of helpful examples.


### Decode an image

```rust
use image;
use matrix_qrcode::{QrVerificationData, DecodingError};

fn main() -> Result<(), DecodingError> {
    let image = image::open("/path/to/my/image.png").unwrap();
    let result = QrVerificationData::from_image(image)?;

    Ok(())
}
```

### Encode into a QR code

```rust
use matrix_qrcode::{QrVerificationData, DecodingError};
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


## License

[Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)
