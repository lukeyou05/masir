set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]
export RUST_BACKTRACE := "full"

clean:
    cargo clean

fmt:
    cargo +nightly fmt
    cargo +stable clippy
    prettier -w .github/
    prettier -w README.md

install:
    cargo +stable install --path . --locked

run:
    cargo run --locked

info $RUST_LOG="info":
    just run

warn $RUST_LOG="warn":
    just run

debug $RUST_LOG="debug":
    just run

trace $RUST_LOG="trace":
    just run
