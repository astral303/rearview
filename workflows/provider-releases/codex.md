# Codex releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo openai/codex --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | select(startswith("rust-v")) | ltrimstr("rust-v")'
```

Codex tags its command-line releases `rust-v<version>`. The filter drops tags
for other packages in the repository, such as `python-v<version>` for the
Python SDK.

## Source

Repository: [openai/codex](https://github.com/openai/codex)

```sh
git clone --no-checkout --filter=blob:none https://github.com/openai/codex
```

Storage paths:

- `codex-rs/rollout`
- `codex-rs/thread-store`
- `codex-rs/state/migrations`
- `codex-rs/protocol/src/protocol.rs`

## Schema version

The number at the front of the newest file in `codex-rs/state/migrations` is
the `_sqlx_migrations` version (`0052_projects_recency.sql` is 52). rearview
checks it against `NEWEST_VERIFIED_MIGRATION` in
`src/history/provider/codex.rs`.

```sh
git ls-tree --name-only <tag>:codex-rs/state/migrations
```
