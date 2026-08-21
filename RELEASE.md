# Releasing

Requires [rust-release-tools](https://github.com/raine/rust-release-tools):

```bash
pipx install git+https://github.com/raine/rust-release-tools.git
```

To release:

```bash
just release
```

This will:

1. Bump version in Cargo.toml
2. Generate changelog entry using Claude
3. Open editor to review changelog
4. Commit, publish to crates.io, tag, and push

## Updating flake.lock

The Nix package reads `Cargo.toml` and `Cargo.lock` directly, so version and Rust
dependency changes do not require a manual `cargoHash` update. When you want to
refresh the pinned nixpkgs input, run:

```bash
./scripts/update-flake.sh
```

This will:

1. Update `flake.lock`
2. Verify the Nix build and binary
3. Stage the lockfile for commit

GitHub Actions runs the Nix build on pull requests, main, and release tags.

## Backfilling changelog

To generate changelog entries for all git tags missing from CHANGELOG.md:

```bash
update-changelog
```

This uses `cc-batch` to process multiple tags in parallel.
