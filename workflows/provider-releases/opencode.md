# OpenCode releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo anomalyco/opencode --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | ltrimstr("v")'
```

OpenCode tags its releases `v<version>`.

## Source

Repository: [anomalyco/opencode](https://github.com/anomalyco/opencode)

```sh
git clone --no-checkout --filter=blob:none https://github.com/anomalyco/opencode
```

Storage paths:

- `packages/core/src/database/migration`
- the code that writes message and part rows: to establish on first review

## Schema version

OpenCode names each migration `<timestamp>_<name>.ts`; the newest file in
`packages/core/src/database/migration` is the schema version. rearview checks
the name without `.ts` against `NEWEST_VERIFIED_MIGRATION` in
`src/history/format/opencode.rs`.

```sh
git ls-tree --name-only <tag>:packages/core/src/database/migration
```
