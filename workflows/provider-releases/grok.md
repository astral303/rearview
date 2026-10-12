# Grok Build releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh api -H "Accept: application/vnd.github.raw" repos/xai-org/grok-build/contents/crates/codegen/xai-grok-shell/CHANGELOG.md | sed -n 's/^# \([0-9][0-9.]*\) .*/\1/p'
```

Grok Build publishes no GitHub releases or tags. Its
[`CHANGELOG.md`](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-shell/CHANGELOG.md)
has one `# <version> — <date>` heading per release, newest first; the filter
prints those versions. The changelog is also the release notes.

## Source

Repository: [xai-org/grok-build](https://github.com/xai-org/grok-build)

```sh
git clone --no-checkout --filter=blob:none https://github.com/xai-org/grok-build
```

With no tags, step 3 is a plain `git fetch <remote>`, and step 4 uses the
commit that first lists a version in place of its tag:

```sh
git log --reverse --format=%H -S "# <version> " -- crates/codegen/xai-grok-shell/CHANGELOG.md | head -1
```

One synced commit can list several releases; review those releases together.

Storage paths:

- `crates/codegen/xai-grok-shell/src/session/storage`
- `crates/codegen/xai-grok-shell/src/session/persistence.rs`
- `crates/codegen/xai-grok-shell/src/session/helpers/session_summary.rs`
- `crates/codegen/xai-grok-shell/src/agent/subagent`

## Schema version

None: rearview checks no Grok schema version.
