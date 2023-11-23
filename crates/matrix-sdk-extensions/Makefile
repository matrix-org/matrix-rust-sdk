build-all-guests: build-timeline-guest

build-timeline-guest: transpile-timeline-guest-to-js
	cd guests/timeline && \
		cargo build --target wasm32-unknown-unknown --release && \
		wasm-tools component new target/wasm32-unknown-unknown/release/timeline.wasm -o ../../timeline.wasm

transpile-timeline-guest-to-js:
	jco transpile --instantiation sync --optimize --no-typescript --no-nodejs-compat --js --out-dir guests/timeline/js/ timeline.wasm
