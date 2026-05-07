use std::env;

fn main() {
    let cxx = env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let lowered = cxx.to_ascii_lowercase();
    if lowered.contains("g++") || lowered.contains("gcc") {
        println!(
            "cargo:warning=LiveKit's WebRTC build is more reliable with clang; \
             try `CC=clang CXX=clang++` if you hit C++ compilation errors."
        );
    }
}
