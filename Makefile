# Inputs default to the parent directory; override with: make run DIR=/path
DIR ?= ..

.PHONY: build check test run report

build:
	cargo build --locked --release

check:
	cargo check --locked
	cargo clippy --locked -- -D warnings

test:
	cargo test --locked

run: build
	./target/release/rallye "$(DIR)"

report: build
	./target/release/rallye report "$(DIR)"
