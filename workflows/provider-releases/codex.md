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
- `codex-rs/history`: the rollout record types and their JSON tags
- `codex-rs/state/migrations`
- `codex-rs/state/src/sqlite.rs`: the state database's file name
- `codex-rs/protocol/src/protocol.rs`: event records
- `codex-rs/protocol/src/models.rs`: response item records

## Schema version

The number at the front of the newest file in `codex-rs/state/migrations` is
the `_sqlx_migrations` version (`0052_projects_recency.sql` is 52). rearview
checks it against `NEWEST_VERIFIED_MIGRATION` in
`src/history/provider/codex.rs`.

```sh
git ls-tree --name-only <tag>:codex-rs/state/migrations
```

A migration matters when it changes the columns read by the two queries beside
`NEWEST_VERIFIED_MIGRATION` (today `threads.id`, `threads.rollout_path`,
`threads.source` and `thread_spawn_edges`). When the review settles every
migration past the pin,
move `NEWEST_VERIFIED_MIGRATION`, the test database's migration row in
`src/history/provider/codex.rs`, and the Schema pin row in PROVIDERS.md's
Codex Storage table to the newest version.
