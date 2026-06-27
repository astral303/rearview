# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# List available commands
default:
    @just --list

# Run format, clippy, tests, and build
check: format clippy test build

# Run Rust checks through checkle
checkle-check: format-check clippy-check test-check

# Run check and fail if there are uncommitted changes (for CI)
check-ci: check
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: check caused uncommitted changes"
        echo "Run 'just check' locally and commit the results"
        git diff --stat
        exit 1
    fi

# Format Rust files
format:
    @cargo fmt --all

# Check Rust formatting through checkle
format-check:
    checkle --label format-check --mode rustfmt -- cargo fmt --all -- --check

# Auto-fix clippy warnings, then fail on any remaining
clippy:
    @cargo clippy --fix --allow-dirty --locked --quiet -- -D clippy::all 2>&1 | { grep -v "^0 errors" || true; }

# Check clippy through checkle
clippy-check:
    checkle --label clippy --mode cargo -- cargo clippy --all-targets --locked --message-format=json -- -D clippy::all

# Build the project
build:
    cargo build --all --locked

# Run tests
test:
    cargo test --all --locked

# Run tests through checkle
test-check:
    checkle --label test --mode cargo -- cargo test --all --locked --message-format=json

# Install release binary globally
install:
    cargo install --offline --path . --locked

# Install debug binary globally via symlink
install-dev:
    cargo build && ln -sf $(pwd)/target/debug/claude-history ~/.cargo/bin/claude-history

# Run the application
run *ARGS:
    cargo run -- "$@"

# Release a new patch version
release:
    @just _release patch

# Internal release helper
_release bump:
    @cargo-release {{bump}}
