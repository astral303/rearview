# OMP releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo can1357/oh-my-pi --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | ltrimstr("v")'
```

OMP tags its releases `v<version>`.

## Source

Repository: [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi)

```sh
git clone --no-checkout --filter=blob:none https://github.com/can1357/oh-my-pi
```

Storage paths:

- `packages/coding-agent/src/session/session-entries.ts`: the session header,
  the `title` slot and every entry type
- `packages/tui/src/chat/messages.ts`: OMP's own message roles, such as
  `bashExecution`, `pythonExecution` and `fileMention`
- `packages/ai/src/types.ts`: user, developer, assistant and tool result
  messages, and their content blocks
- `packages/coding-agent/src/tools/builtin-names.ts`,
  `packages/coding-agent/src/tools/index.ts` and
  `packages/coding-agent/src/tools/settings.ts`: the built-in tools, their
  former names, and the ones on by default
- `packages/coding-agent/src/edit/schemas.ts`,
  `packages/coding-agent/src/edit/settings.ts` and
  `packages/coding-agent/src/utils/edit-mode.ts`: the edit modes, their input,
  the default mode and the models that default to `replace`
- `packages/coding-agent/src/session/session-paths.ts` and
  `packages/utils/src/dirs.ts`: the session directory and its overrides
- `packages/coding-agent/src/task/executor.ts`: sub-agent transcripts in the
  artifacts directory
- `packages/coding-agent/src/advisor/transcript-recorder.ts`: advisor
  transcripts in the same directory
- `packages/coding-agent/src/session/session-manager.ts`: the session's token
  totals (`entryUsage`)
- `packages/coding-agent/src/modes/utils/ui-helpers.ts`: the replay of a
  reopened session (`addMessageToChat`)
- `packages/coding-agent/src/session/session-context.ts`: the session state
  OMP restores and does not show

## Dropped-records report

- Kinds a review decided against are listed in `SKIPPED_FOR_OMP` in
  `src/history/format/pi_log.rs` and counted as `skipped`.
- When a review decides against a kind, add it to `SKIPPED_FOR_OMP`'s
  `entry_types` or `message_roles`.
- Pi shares the reader but keeps its own lists; change them only in a Pi
  review.

## Schema version

None: rearview checks no OMP schema version.
