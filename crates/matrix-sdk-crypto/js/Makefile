build:
	RUSTFLAGS='-C opt-level=z' wasm-pack build --release --target nodejs --out-dir ./js/pkg ../ --features js

test:
	node --test ../tests/js/**.js
