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
