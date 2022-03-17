fn main() {
    uniffi_build::generate_scaffolding("./src/api.udl").expect("Building the UDL file failed");
}
