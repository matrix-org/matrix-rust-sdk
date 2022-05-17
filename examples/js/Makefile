build:
	RUSTFLAGS='-C opt-level=z' wasm-pack build --release --target nodejs ../../crates/matrix-sdk-crypto --features js

test:
	node --test ../../crates/matrix-sdk-crypto/tests/js/**.js
