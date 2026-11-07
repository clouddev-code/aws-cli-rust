# The sandbox used during development cannot write to ~/.cargo, so keep
# cargo state inside the project. Override with `make CARGO_HOME=... test`.
CARGO_HOME ?= $(CURDIR)/.cargo-home
export CARGO_HOME

.PHONY: build test test-debug release fmt clippy fetch-upstream update-upstream

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

# reference/aws-cli at the release in upstream.lock (needed to build with
# embedded models and for the data-driven tests).
fetch-upstream:
	scripts/upstream.sh fetch

# Move to the latest aws-cli release (or TAG=2.x.y) and run the tests.
update-upstream:
	scripts/upstream.sh update $(TAG)
	$(MAKE) test
