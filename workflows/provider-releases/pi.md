# Pi releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo earendil-works/pi --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | ltrimstr("v")'
```

Pi tags its releases `v<version>`.

## Source

Repository: [earendil-works/pi](https://github.com/earendil-works/pi)

```sh
git clone --no-checkout --filter=blob:none https://github.com/earendil-works/pi
```

Storage paths:

- `packages/coding-agent/src/core/session-manager.ts`: the session header,
  every entry type, the session directory and the file name
- `packages/coding-agent/src/core/messages.ts`: Pi's own message roles, such
  as `bashExecution` and `custom`
- `packages/ai/src/types.ts`: user, assistant, tool result and system
  messages, and their content blocks
- `packages/coding-agent/src/core/tools`: the built-in tools and their
  arguments
- `packages/coding-agent/src/config.ts`: the `PI_CODING_AGENT_DIR` override
- `packages/coding-agent/src/main.ts`: the session directory, from
  `--session-dir`, `PI_CODING_AGENT_SESSION_DIR` or the `sessionDir`
  setting, in that order
- `packages/coding-agent/src/modes/interactive/interactive-mode.ts`: the
  replay of a reopened session (`renderSessionEntries`)

## Dropped-records report

- Kinds a review decided against are named in the Pi reader and counted as
  `skipped`.
- When a review decides against a kind, add it to Pi's list for its field.
- OMP shares the reader but keeps its own lists; change them only in an OMP
  review.

## Schema version

None: rearview checks no Pi schema version.
