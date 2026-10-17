# Build the web interface first so it is embedded into the binary.
.PHONY: all ui build release test check run clean

all: build

ui:
	cd ui && npm ci && npm run build

build: ui
	cargo build

release: ui
	cargo build --release

test:
	cargo test

check:
	cargo clippy --all-targets
	cargo fmt --check

run:
	cargo run -- serve

clean:
	cargo clean
	rm -rf ui/build ui/.svelte-kit
