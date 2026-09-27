export PATH := $(HOME)/.cargo/bin:$(PATH)

.PHONY: setup build test fmt clippy msrv nostd miri

build:
	cargo build --verbose

setup:
	rustup toolchain install stable --component clippy,rustfmt
	rustup toolchain install 1.65.0
	rustup toolchain install nightly --component miri
	cargo +stable install cargo-hack --locked
	cargo +nightly miri setup

ci: build test fmt clippy msrv nostd miri

test:
	cargo test --verbose

fmt:
	cargo fmt -- --check

clippy:
	cargo clippy -- -D warnings

msrv:
	cargo hack check --rust-version --workspace --lib --ignore-private

nostd:
	RUSTFLAGS='-C panic=abort' cargo build --no-default-features --lib

miri: export MIRIFLAGS = -Zmiri-ignore-leaks # ignore detached background threads
#                                              - at least futures-timer spawns one
#   cargo +nightly miri test --test mpmc_spam # takes forever under Miri
miri:
	cargo +nightly miri setup
	cargo +nightly miri test --test borrow_basic
	cargo +nightly miri test --test borrow_dyn
	cargo +nightly miri test --test abort
	cargo +nightly miri test --test borrow_basic_mt
	cargo +nightly miri test --test borrow_corner_cases
