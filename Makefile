# Single check entry point: format, types (clippy), Rust tests and harness tests.
# Each part runs once the product it checks exists; with no product yet, check passes.
.PHONY: check

check:
	@if [ -f Cargo.toml ]; then \
		cargo fmt --all -- --check && \
		cargo clippy --workspace --all-targets -- -D clippy::correctness -D clippy::suspicious && \
		cargo test --workspace; \
	else \
		echo "check: Cargo.toml not present yet, Rust skipped"; \
	fi
	@if [ -d harness/tests ]; then \
		python3 -m unittest discover -s harness/tests -t .; \
	else \
		echo "check: harness/tests not present yet, harness skipped"; \
	fi
