release: build
    cargo build --release

build:
    cargo clippy --bin "nevermail-tui" -p nevermail-tui
    cargo build
    cargo test

