use bindgen::Bindings;
use std::env;
use std::path::PathBuf;

fn build(file: &str) -> Result<Bindings, ()> {
    println!("cargo:rustc-link-lib=olm");

    bindgen::Builder::default()
        .rustfmt_bindings(true)
        .header(file)
        .generate()
}

fn main() {
    let bindings = build("src/wrapper.h").expect("Unable to build bindings");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
