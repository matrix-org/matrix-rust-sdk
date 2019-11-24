test:
	cargo test

coverage:
	cargo tarpaulin

clean:
	cargo clean

format:
	cargo fmt

.PHONY: clean test coverage
