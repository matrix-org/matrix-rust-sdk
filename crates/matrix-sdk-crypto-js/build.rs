#[cfg(feature = "nodejs")]
fn main() {
    napi_build::setup();
}

#[cfg(not(feature = "nodejs"))]
fn main() {}
