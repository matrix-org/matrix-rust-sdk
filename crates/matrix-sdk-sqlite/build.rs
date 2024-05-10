fn main() {
    // Prevent unnecessary rerunning of this build script
    println!("cargo:rerun-if-changed=build.rs");

    // Prevent nightly CI from erroring on tarpaulin_include attribute
    println!("cargo:rustc-check-cfg=cfg(tarpaulin_include)");
}
