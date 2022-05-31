build:
	RUSTFLAGS='-C opt-level=z' wasm-pack build --release --target nodejs --out-name matrix_sdk_crypto --out-dir ./js/pkg ../ --features js

test:
	node --test ../tests/js/**.js
