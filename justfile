# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# Crate and binary name of this fork. The `claude-history` crate on crates.io
# is upstream's, so this placeholder is deliberately not a valid crate name:
# recipes that depend on it fail until a name is chosen here and in Cargo.toml.
crate_name := "~~TODO^pkg^name~~"

# List available commands
default:
    @just --list

# Run project checks through checkle
check:
    checkle run all

# Run project checks through checkle
checkle-check: check

# Run check and fail if there are uncommitted changes for CI
check-ci: check
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: check caused uncommitted changes"
        echo "Run 'just check' locally and commit the results"
        git diff --stat
        exit 1
    fi

# Install shims into the Git hooks directory
install-hooks:
    scripts/install-git-hook-shims

# Check Rust formatting through checkle
format:
    checkle run format-check

# Check Rust formatting through checkle
format-check: format

# Check clippy through checkle
clippy:
    checkle run clippy

# Check clippy through checkle
clippy-check: clippy

# Check the build through checkle
build:
    checkle run build

# Run tests through checkle
test:
    checkle run test

# Run tests through checkle
test-check: test

# Install release binary globally
install:
    cargo install --offline --path . --locked

# Install debug binary globally via symlink
install-dev: _check-crate-name
    cargo build && ln -sf $(pwd)/target/debug/{{crate_name}} ~/.cargo/bin/{{crate_name}}

# Run the application
run *ARGS:
    cargo run -- "$@"

# Release a new patch version
release:
    @just _release patch

# Internal release helper
_release bump: _check-crate-name
    @cargo-release {{bump}}

# Fail unless the package in Cargo.toml is named crate_name
_check-crate-name:
    @cargo pkgid "{{crate_name}}" >/dev/null
