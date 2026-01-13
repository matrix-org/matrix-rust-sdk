# matrix-sdk-rtc-livekit

LiveKit SDK integration for `matrix-sdk-rtc`.

## Toolchain setup (Linux)

The LiveKit Rust SDK builds native WebRTC components. On Linux, the build is
most reliable with clang and libc++.

Install libc++ and libc++abi (Ubuntu/Debian):

```bash
sudo apt-get install -y libc++-dev libc++abi-dev
```

Build with clang + libc++:

```bash
CC=clang CXX=clang++ CXXFLAGS="-stdlib=libc++" \
  cargo build -p matrix-sdk-rtc-livekit
```

If clang still picks GCC's headers, force the libc++ include path:

```bash
CC=clang CXX=clang++ CXXFLAGS="-stdlib=libc++ -isystem /usr/include/c++/v1" \
  cargo build -p matrix-sdk-rtc-livekit
```

### Linker errors about `std::__1` symbols

If you see linker errors that mention `std::__1` (libc++), it means some native
objects were built against libc++ but the final link step is using libstdc++.

Try one of the following:

- Ensure libc++ and libc++abi are installed (see above).
- Explicitly link libc++/libc++abi when building:

```bash
RUSTFLAGS="-l c++ -l c++abi" cargo build -p matrix-sdk-rtc-livekit
```
