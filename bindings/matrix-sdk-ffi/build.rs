fn main() {
    uniffi::generate_scaffolding("./src/api.udl").expect("Building the UDL file failed");
}
