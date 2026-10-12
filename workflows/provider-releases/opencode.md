# OpenCode releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
git ls-remote --tags --refs https://github.com/anomalyco/opencode 'v*' | sed -n 's|.*refs/tags/v\([0-9][0-9.]*\)$|\1|p' | sort -V -r
```

OpenCode tags its releases `v<version>`. OpenCode 1 releases also appear on
GitHub's releases page; OpenCode 2 releases are tags and npm versions of
`@opencode/cli` only, so the listing reads tags. It prints both lines, newest
first, and needs no `--limit`.

OpenCode ships two lines at once. Review 2.x releases newer than "Reviewed
through", and 1.x releases newer than 1.19.0, the newest 1.x reviewed; update
that version here after each review.

## Source

Repository: [anomalyco/opencode](https://github.com/anomalyco/opencode)

```sh
git clone --no-checkout --filter=blob:none https://github.com/anomalyco/opencode
```

Storage paths:

- `packages/core/src/database/migration`
- `packages/schema/src/session-message.ts`: OpenCode 2's message types and
  their JSON
- `packages/core/src/database/v1-migration.bun.ts`: how OpenCode 2 copies an
  OpenCode 1 session into its own tables

## Schema version

OpenCode names each migration `<timestamp>_<name>.ts`; the newest file in
`packages/core/src/database/migration` is the schema version. rearview checks
the name without `.ts` against `NEWEST_VERIFIED_MIGRATION` in
`src/history/format/opencode.rs`.

```sh
git ls-tree --name-only <tag>:packages/core/src/database/migration
```
