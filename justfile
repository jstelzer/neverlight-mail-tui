release: build
    cargo build --release

build:
    cargo clippy --bin "neverlight-mail-tui" -p neverlight-mail-tui
    cargo build
    cargo test

