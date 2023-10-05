build-all-guests: build-timeline-guest

build-timeline-guest:
	cd guests/timeline && \
		cargo build --target wasm32-unknown-unknown --release && \
		wasm-tools component new target/wasm32-unknown-unknown/release/timeline.wasm -o ../../timeline.wasm

transpile-timeline-guest-to-js:
	 jco transpile --instantiation --optimize --no-typescript --out-dir guests/timeline/js/ timeline.wasm
