# Glossary

One word for each concept, in the interface, docs, comments, test names and
identifiers.

| Term | Meaning | Replaces |
|---|---|---|
| agent | A coding agent whose sessions rearview reads, such as Claude Code or Codex. In code, an agent's provider reads its sessions; see [PROVIDERS.md](PROVIDERS.md). | |
| ignored | Skipped and reported: `Codex │ 12 ignored: compressed sessions unsupported`. | not read, unread |
| limit | The most a count, size or length may reach: a line limit, a budget. | cap, bound |
| nested sub-agent | A sub-agent's own sub-agent. | grandchild |
| permanent error | Any error that is not a transient error. | |
| session | One conversation with an agent, as the list shows it. | rollout (kept in code as the name of Codex's transcript file) |
| session ID | The identifier an agent gives a session, such as a UUID or OpenCode's `ses_…`. Written `ID` in prose; field names and flags keep their spelling (`id`, `--show-id`). | id |
| sub-agent | A session or thread an agent ran under another session; `subagent` in identifiers. | child, descendant, spawned |
| transcript | The record an agent writes for a session: a file, or for OpenCode the session's rows in its database. | |
| transient error | An error that can clear on its own: a lock, a busy database, interrupted I/O. | temporary error, locked |
| truncated | Shortened to a limit: a tool result to its line limit, an `agent read` message to its `--budget`. The viewer's `tools·trn` mode truncates tool results. | cut, capped, clipped, abbreviated |
