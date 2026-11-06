# The sandbox used during development cannot write to ~/.cargo, so keep
# cargo state inside the project. Override with `make CARGO_HOME=... test`.
CARGO_HOME ?= $(CURDIR)/.cargo-home
export CARGO_HOME

.PHONY: build test test-debug release fmt clippy

build:
	cargo build

test:
	cargo test --release

test-debug:
	cargo test

release:
	cargo build --release

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets
