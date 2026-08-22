# Releasing

Requires a [crates.io](https://crates.io) token (`cargo login`).
Uses [cargo-release](https://github.com/crate-ci/cargo-release), provided
by `mise install`. 

1. Write the changelog entry. (The release replaces the `## Unreleased`
   heading in `CHANGELOG.md` with the version and date, and refuses to run unless
   exactly one such heading exists.)

2. Preview or release: (default `<bump>` is `patch`)

   ```bash
   cargo release [<bump>]   # preview only: the dry run is cargo-release's default
   just release [<bump>]    # bump, commit, publish, tag, push
   ```

A release:

* Checks that the tree is clean, the branch is `main`, and `origin` is not
  ahead. 
* Bumps the version in `Cargo.toml` and `Cargo.lock` and retitles the changelog
  heading.
* Commits `release vX.Y.Z`, publishes to crates.io, tags `vX.Y.Z`, and pushes.

The tag push runs `.github/workflows/release.yml`:
* builds binaries for MacOS, Linux and Windows,
* attaches them to a GitHub release,
* now available to `rearview update` and `scripts/install.sh`
