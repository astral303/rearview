# Kimi Code releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo MoonshotAI/kimi-code --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | select(startswith("@moonshot-ai/kimi-code@")) | ltrimstr("@moonshot-ai/kimi-code@")'
```

Kimi Code tags its releases `@moonshot-ai/kimi-code@<version>`. The filter
drops tags for any other package in the repository.

## Source

Repository: [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code)

```sh
git clone --no-checkout --filter=blob:none https://github.com/MoonshotAI/kimi-code
```

Storage paths:

- `packages/agent-core-v2/docs/wire-manifest.d.ts`: every `wire.jsonl` record
  type and payload, and the wire protocol version
- `packages/agent-core-v2/src/wire`: the wire writer and its protocol migrations
- `packages/agent-core-v2/src/session/sessionMetadata`: `state.json`
- `packages/agent-core-v2/src/session/agentLifecycle/subagentMetadata.ts`: the
  sub-agent entries in `state.json`
- `packages/agent-core-v2/src/workspace/sessionLifecycle`: the session
  directory layout and `session_index.jsonl`
- `packages/agent-core-v2/src/agent/replayBuilder` and
  `apps/kimi-code/src/tui/controllers/session-replay.ts`: the replay of a
  reopened session

## Schema version

None: rearview checks no Kimi schema version.
