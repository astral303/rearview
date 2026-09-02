# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

# Crate and binary name. `_check-crate-name` fails when Cargo.toml disagrees,
# so `install-dev` never links a binary that does not exist.
crate_name := "rearview"

# List available commands
default:
    @just --list

# Run every check in the `all` group of checkle.toml, after the toolchain pin check
check:
    mise run check-toolchain-pin
    checkle run all

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

# Report Rust formatting violations without rewriting files
format-check:
    checkle run format-check

# Run clippy lints
clippy:
    checkle run clippy

# Build all targets
build:
    checkle run build

# Run the test suite
test:
    checkle run test

# Install release binary globally
install:
    cargo install --offline --path . --locked

# Install debug binary globally via symlink
install-dev: _check-crate-name
    cargo build && ln -sf $(pwd)/target/debug/{{crate_name}} ~/.cargo/bin/{{crate_name}}

# Run the application
run *ARGS:
    cargo run -- "$@"

# Run `cargo release <bump>` without --execute to preview the same steps.
[doc("Bump (patch, minor, major), retitle the changelog, commit, publish, tag, push")]
release bump="patch":
    cargo release {{bump}} --execute

# Fail unless the package in Cargo.toml is named crate_name
_check-crate-name:
    @cargo pkgid "{{crate_name}}" >/dev/null
