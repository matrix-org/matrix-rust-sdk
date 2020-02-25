test:
	cargo test --features encryption

coverage:
	cargo tarpaulin --features encryption -v

clean:
	cargo clean

format:
	cargo fmt

.PHONY: clean test coverage
