# List supported development commands.
default:
    @just --list

# Compile the CLI using the committed dependency lockfile.
build:
    cargo build --locked

# Check formatting, compiler warnings, and safety tests.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

# Format Rust source.
fmt:
    cargo fmt

# Rehearse against disposable Docker services, without existing project data.
rehearse: build
    python3 tests/docker_rehearsal.py

# Run the CLI, for example: just cli --help.
cli *args:
    cargo run --locked -- {{args}}
