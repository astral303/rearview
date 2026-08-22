# Releasing

Requires [cargo-release](https://github.com/crate-ci/cargo-release)
(`cargo install cargo-release`, or `mise use cargo-release`) and a
crates.io token (`cargo login`).

Write the changelog entry first. The release replaces the `## Unreleased`
heading in CHANGELOG.md with the version and date, and refuses to run unless
exactly one such heading exists.

```bash
cargo release minor   # preview only: the dry run is cargo-release's default
just release minor    # bump, commit, publish, tag, push
```

`just release` alone bumps the patch version. A release:

1. Checks that the tree is clean, the branch is `main`, and `origin` is not
   ahead.
2. Bumps the version in Cargo.toml and Cargo.lock and retitles the changelog
   heading.
3. Commits `release vX.Y.Z`, publishes to crates.io, tags `vX.Y.Z`, and pushes.

The tag push runs `.github/workflows/release.yml`, which builds the macOS and
Linux binaries and attaches them to a GitHub release; `rearview update` and
`scripts/install.sh` install from there.
