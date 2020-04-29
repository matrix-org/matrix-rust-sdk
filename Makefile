all: build

build:
	cargo build
test:
	cargo test

coverage:
	cargo tarpaulin -v

clean:
	cargo clean

format:
	cargo fmt

.PHONY: clean test coverage
