# Rust project checks

set positional-arguments
set shell := ["bash", "-euo", "pipefail", "-c"]

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

# Provision the shared CuaBot verification environment
cua-sandbox-setup:
    scripts/cua-sandbox setup

# Start an isolated CuaBot verification session
cua-sandbox-start session:
    scripts/cua-sandbox start {{session}}

# Build, deploy, and seed claude-history in a CuaBot session
cua-sandbox-prepare session:
    scripts/cua-sandbox prepare {{session}}

# Launch claude-history in a CuaBot session
cua-sandbox-launch session *ARGS:
    scripts/cua-sandbox launch {{session}} {{ARGS}}

# Launch the --show-id command-substitution check in a CuaBot session
cua-sandbox-verify-show-id session:
    scripts/cua-sandbox verify-show-id {{session}}

# Send a tmux key to the TUI in a CuaBot session
cua-sandbox-key session key:
    scripts/cua-sandbox key {{session}} {{key}}

# Stop an isolated CuaBot verification session
cua-sandbox-stop session:
    scripts/cua-sandbox stop {{session}}

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
