all: build

build:
	cargo build --features 'encryption sqlite-cryptostore'
test:
	cargo test --features 'encryption sqlite-cryptostore'

coverage:
	cargo tarpaulin --features 'encryption sqlite-cryptostore' -v

clean:
	cargo clean

format:
	cargo fmt

.PHONY: clean test coverage
