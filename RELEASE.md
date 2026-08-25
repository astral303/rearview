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

## Checking a build before tagging

`release.yml` runs `verify-release-binary` against each packaged binary, so a
release build checks that semantic search works on that platform rather than
only that `--version` answers.

To get that without creating a release, run the workflow by hand from the branch
you want and leave both inputs empty. It builds and checks all three platforms
and uploads the archives as workflow artifacts. A release is only created when
you set `publish` **and** give a `tag`; a tag push always publishes.

To check an archive you already have:

```bash
REARVIEW_BIN=<extracted>/rearview mise run verify-release-binary
```

It seeds a throwaway history and points every provider at it, so none of your
conversations are read or embedded. The embedding model downloads on the first
run and is kept for later ones.

## Homebrew tap

The release does not update the tap. Once the workflow has attached the
archives:

```bash
mise run homebrew-formula   # write Formula/rearview.rb in the tap and commit it
mise run homebrew-push      # publish it
```

`homebrew-formula` takes the version from `Cargo.toml` and both hashes from the
release's `.sha256` assets, renders
[`scripts/homebrew-formula.rb.tmpl`](scripts/homebrew-formula.rb.tmpl) into the
tap, and stops if the assets are not attached yet. Edit the template, never the
generated formula.

`HOMEBREW_TAP_DIR` points at the tap checkout and defaults to
`../homebrew-rearview`. `RELEASE_VERSION` overrides the tag.

To install the formula before publishing it, `mise run brew-formula`.

Scoop needs nothing here. Its bucket updates itself with `checkver`.
